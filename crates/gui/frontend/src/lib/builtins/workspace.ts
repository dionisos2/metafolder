// The workspace builtins (doc "Workspaces", doc "Saved workspaces"):
// `workspace:*`.

import { savedWorkspaces } from "../builtinArgs";
import { invoke } from "../ipc";
import { status } from "../status";
import { confirmClosing } from "../unsavedEdits";
import type { Builtins } from "./types";


export const workspaceBuiltins: Builtins = {
  "workspace:new": async (args) => {
    // `self-repo`, `none` or a repo uuid, read by the GUI state (doc
    // "workspace:new").
    await invoke("workspace_new", { repo: args[0] });
  },
  "workspace:fork": async (_args, context) => {
    // A copy of the context's workspace, in a new tab (doc "Workspaces").
    const { ws } = context;
    if (ws) await invoke("workspace_fork", { wsId: ws });
  },
  "workspace:send": async (args) => {
    // `workspace:send <id> <key> <value…>`: the value is JSON when it parses,
    // the text otherwise — the last argument absorbs the remaining tokens,
    // so a query needs no quoting.
    const [target, key, ...rest] = args;
    if (!target || !key) return;
    const text = rest.join(" ");
    let value: unknown = text;
    try {
      value = JSON.parse(text);
    } catch {
      /* plain text */
    }
    await invoke("workspace_send", { wsId: target, vars: { [key]: value } });
  },
  "workspace:close": async (_args, context) => {
    const { ws } = context;
    if (!confirmClosing(ws ?? null)) return;
    // The context's workspace, which a command may have moved to
    // (`mf.withContext`): the focused one only without a context.
    if (ws) await invoke("workspace_close_ws", { wsId: ws });
    else await invoke("workspace_close");
  },
  "workspace:rename": async (args, context) => {
    // The name was collected by dispatch (inline or in the minibuffer); as
    // the last argument it absorbs the remaining tokens, so a name may
    // contain spaces without quoting.
    const { ws } = context;
    const name = args.join(" ");
    if (ws) await invoke("workspace_rename", { wsId: ws, name });
  },
  // Saved workspaces (doc "Saved workspaces"). The name is the last
  // argument and absorbs the remaining tokens, as workspace:rename's does.
  "workspace:save": async (args, context) => {
    const { ws } = context;
    const [kind, ...rest] = args;
    const name = rest.join(" ");
    if (!ws || !kind || !name) return;
    const exists = (await savedWorkspaces()).some((w) => w.name === name);
    if (
      exists &&
      !window.confirm(`Replace the workspace saved as "${name}"?`)
    )
      return;
    await invoke("saved_workspace_save", { wsId: ws, kind, name });
    await status(`workspace saved as "${name}" (${kind})`, "info");
  },
  "workspace:open": async (args, context) => {
    const { ws } = context;
    const [target, ...rest] = args;
    const name = rest.join(" ");
    if (!ws || !target || !name) return;
    await invoke("saved_workspace_open", { wsId: ws, target, name });
  },
  "workspace:set-default": async (args, context) => {
    const { ws } = context;
    const name = args.join(" ");
    if (!ws || !name) return;
    await invoke("saved_workspace_set_default", { wsId: ws, name });
    await status(`"${name}" is the repository's default workspace`, "info");
  },
  "workspace:unset-default": async (_args, context) => {
    const { ws } = context;
    if (!ws) return;
    await status(
      (await invoke<boolean>("saved_workspace_unset_default", { wsId: ws }))
        ? "the repository has no default workspace any more"
        : "the repository had no default workspace",
      "info",
    );
  },
  "workspace:delete-saved": async (args, context) => {
    const { ws } = context;
    const name = args.join(" ");
    if (!ws || !name) return;
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
  },
  "workspace:goto": async (args) => {
    // The 1-based workspace position is the parameter (no longer baked
    // into the command name). Moves BOTH panels.
    const n = Number(args[0]);
    if (Number.isInteger(n) && n > 0) await invoke("workspace_goto", { n });
    else await status(`not a workspace number: "${args[0] ?? ""}"`);
  },
  // Bare, both panels move together so the two slots never drift onto
  // different workspaces unnoticed; `slot` moves only the focused one.
  "workspace:next": async (args) => {
    await invoke(
      args[0] === "slot" ? "workspace_next_in_slot" : "workspace_next",
    );
  },
  "workspace:prev": async (args) => {
    await invoke(
      args[0] === "slot" ? "workspace_prev_in_slot" : "workspace_prev",
    );
  },
};
