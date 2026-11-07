import { registerArgs } from "./commandArgs";
import { NEW_KINDS } from "./fileCommands";
import { GENERIC_COMMANDS, FIND_MODES, SELF, ZOOM_DIRECTIONS, findCandidates, functionProvider } from "./panelFunctions";
import { activeQuestion, focusedContext, focusedWs, store, workspaceById } from "./store.svelte";
import { TARGETS } from "./targets";
import { type ArgSpec } from "./commandArgs";
import { invoke } from "./ipc";
import { neededFunctions, resolvePanel } from "./panelFunctions";
import { slotPayload } from "./store.svelte";
import { type ExecContext } from "./types";

// Saved workspaces (doc "Saved workspaces"): the names saved in the focused
// workspace's repository are the completions of every command naming one.
type SavedWorkspaceEntry = {
  name: string;
  kind: string | null;
  default: boolean;
};

export async function savedWorkspaces(): Promise<SavedWorkspaceEntry[]> {
  const ws = focusedWs();
  if (!ws) return [];
  try {
    return await invoke<SavedWorkspaceEntry[]>("saved_workspace_list", {
      wsId: ws,
    });
  } catch {
    return []; // no repository: nothing to complete
  }
}

const savedWorkspaceNames = async () =>
  (await savedWorkspaces()).map((w) => w.name);

// Generic panel commands (doc "Generic panel commands"): the first argument
// is the panel they act on — a panel type, or `self` for the focused one. The
// candidates are the panel types providing what the command needs.
function panelArg(command: string): ArgSpec {
  return {
    name: "panel",
    prompt: () => `Which panel? (${SELF}, or a panel type)`,
    complete: (_partial, prior) => {
      const needed = neededFunctions(command, prior.slice(1)) ?? [];
      const provider = functionProvider();
      const types = store.panelTypes.filter((type) =>
        needed.every((fn) => provider?.provided(type)?.has(fn)),
      );
      return [SELF, ...types];
    },
  };
}

/** The panel type a generic command's `panel` argument designates, in `context`. */
export function genericPanel(arg: string | undefined, context: ExecContext): string {
  return resolvePanel(
    arg,
    context.slot ? slotPayload(context.slot).panel_type : null,
  );
}

const WATCH_ACTIVITY_OPERATIONS = ["reset"];

// ── Builtin argument declarations (doc "Interactive command arguments") ─────────────────
// Every argument a builtin takes is declared here, like a panel command's, so
// a missing one is collected in the minibuffer — with its completions — instead
// of reaching the handler and failing there. `panel:open` typed bare asks which
// setting, then which panel type; `panel:open here` supplies the first and asks
// only the second. A required argument is asked for whenever the invocation
// does not supply it; an `optional` one is the trailing modifier whose absence
// is itself the documented meaning (`workspace:next` moves both panels,
// `workspace:next slot` the focused one only) and is never asked for.

registerArgs("command-input:focus", [
  {
    name: "mode",
    prompt: () => "Which mode? (command / bash)",
    complete: () => ["command", "bash"],
  },
  {
    name: "clear",
    optional: true,
    prompt: () => "Empty the line first? (clear)",
  },
]);

registerArgs("editing:goto", [
  {
    name: "target",
    prompt: () => "Move the cursor where? (line-start / line-end)",
    complete: () => ["line-start", "line-end"],
  },
]);

registerArgs("workspace:new", [
  {
    name: "repo",
    optional: true,
    prompt: () => "On which repository? (empty: none)",
  },
]);

registerArgs("workspace:send", [
  {
    name: "workspace",
    prompt: () => "To which workspace? (its id)",
    complete: () => store.workspaces.map((w) => w.id),
  },
  { name: "variable", prompt: () => "Which variable?" },
  { name: "value", prompt: () => "Its value (JSON, or text):" },
]);

registerArgs("macro:save", [
  { name: "name", prompt: () => "Save the macro as user:…? (a name)" },
]);

registerArgs("workspace:rename", [
  {
    name: "name",
    prompt: () => "Rename the workspace to:",
    initial: () => workspaceById(focusedWs())?.name ?? "",
  },
]);

registerArgs("workspace:save", [
  {
    name: "kind",
    prompt: () => "Save what? (layout / work)",
    complete: () => ["layout", "work"],
  },
  {
    name: "name",
    prompt: () => "Save as? (a saved name is replaced)",
    complete: savedWorkspaceNames,
  },
]);

registerArgs("workspace:open", [
  {
    name: "target",
    prompt: () => "Open where? (here / new)",
    complete: () => ["here", "new"],
  },
  {
    name: "name",
    prompt: () => "Open which saved workspace?",
    complete: savedWorkspaceNames,
  },
]);

registerArgs("workspace:set-default", [
  {
    name: "name",
    prompt: () => "Which saved workspace becomes the default?",
    complete: savedWorkspaceNames,
  },
]);

registerArgs("workspace:delete-saved", [
  {
    name: "name",
    prompt: () => "Delete which saved workspace?",
    complete: savedWorkspaceNames,
  },
]);

registerArgs("workspace:goto", [
  {
    name: "workspace",
    prompt: () => "Go to which workspace?",
    complete: () => store.workspaces.map((_, index) => String(index + 1)),
  },
]);

registerArgs("workspace:next", [
  {
    name: "slot",
    optional: true,
    prompt: () => "Scope? (slot: the focused slot only)",
  },
]);

registerArgs("workspace:prev", [
  {
    name: "slot",
    optional: true,
    prompt: () => "Scope? (slot: the focused slot only)",
  },
]);

registerArgs("panel:toggle", [
  {
    name: "flag",
    prompt: () => "Which flag? (split / fullscreen)",
    complete: () => ["split", "fullscreen"],
  },
]);

registerArgs("file:new", [
  panelArg("file:new"),
  {
    name: "kind",
    prompt: () => `Create what? (${NEW_KINDS.join(" / ")})`,
    complete: () => [...NEW_KINDS],
  },
]);

registerArgs("view:zoom", [
  panelArg("view:zoom"),
  {
    name: "direction",
    prompt: () => `Zoom how? (${ZOOM_DIRECTIONS.join(" / ")})`,
    complete: () => [...ZOOM_DIRECTIONS],
  },
]);

registerArgs("cursor:find", [
  panelArg("cursor:find"),
  {
    name: "mode",
    prompt: () => `Find how? (${FIND_MODES.join(" / ")})`,
    complete: () => [...FIND_MODES],
  },
  {
    name: "entry",
    prompt: () => "Go to entry:",
    complete: async (_partial, prior) => {
      const ws = focusedWs();
      const provider = functionProvider();
      if (!ws || !provider) return [];
      const type = genericPanel(prior[0], focusedContext());
      return findCandidates(await provider.get(ws, type));
    },
    // Shift-Enter stops on what is found, even a directory a walk would enter.
    alternate: {
      hint: "select",
      prior: (prior) => [prior[0] ?? SELF, "select"],
    },
  },
]);

registerArgs("panel:focus", [
  {
    name: "slot",
    prompt: () => "Focus which slot? (next / left / right)",
    complete: () => ["next", "left", "right"],
  },
]);

registerArgs("panel:open", [
  {
    name: "target",
    prompt: () => `Open where? (${TARGETS.join(" / ")})`,
    complete: () => [...TARGETS],
  },
  {
    name: "type",
    prompt: () => "Which panel type?",
    complete: () => [...store.panelTypes],
  },
]);

registerArgs("mf:duplicate", [
  {
    name: "operation",
    prompt: () => "Which operation? (scan)",
    complete: () => ["scan"],
  },
]);

registerArgs("mf:watch-activity", [
  {
    name: "operation",
    prompt: () => `Which operation? (${WATCH_ACTIVITY_OPERATIONS.join(" / ")})`,
    complete: () => [...WATCH_ACTIVITY_OPERATIONS],
  },
]);

registerArgs("daemon:set-url", [
  {
    name: "url",
    prompt: () => "New daemon URL:",
    initial: () => store.daemonUrl,
  },
]);

registerArgs("answer:send", [
  {
    name: "value",
    prompt: () => "Answer with:",
    complete: () => [...(activeQuestion()?.keys ?? [])],
  },
]);

registerArgs("help:open", [
  { name: "topic", optional: true, prompt: () => "Help on what?" },
]);

registerArgs("help:command", [
  {
    name: "command",
    prompt: () => "Help on which command?",
    complete: () => [...new Set(store.commands.map((c) => c.name))].sort(),
  },
]);

registerArgs("find:open", [
  { name: "text", optional: true, prompt: () => "Find what?" },
]);

registerArgs("script:stop", [
  {
    name: "task",
    optional: true,
    prompt: () => "Stop which script? (empty: the one asking)",
  },
]);

registerArgs("script:kill", [
  {
    name: "task",
    optional: true,
    prompt: () => "Kill which script? (empty: the one asking)",
  },
]);

for (const command of GENERIC_COMMANDS) {
  if (!["cursor:find", "view:zoom", "file:new"].includes(command)) {
    registerArgs(command, [panelArg(command)]);
  }
}
