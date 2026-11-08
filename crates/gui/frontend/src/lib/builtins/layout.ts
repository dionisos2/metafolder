// The layout builtins (doc "Panel slots and layout"): `panel:*`, `slot:show`,
// the find bar and hints, the value picker's answers, and the window's own
// (`quit`, `devtools:open`, fullscreen).

import { closeFind, openFind, stepFind } from "../find";
import { openHints } from "../hints";
import { invoke } from "../ipc";
import { status } from "../status";
import { slotPayload, store } from "../store.svelte";
import { isTarget, resolveTarget, TARGETS } from "../targets";
import { confirmClosing } from "../unsavedEdits";
import type { Builtin, Builtins } from "./types";

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

const pickAnswer: Builtin = async (_args, _context, name) => {
  // Hands the focused picker's selection back to the calling form (confirm)
  // or abandons it (cancel). Best-effort: a stray press outside a picker is
  // a silent no-op rather than an error toast.
  try {
    await invoke(name === "pick:confirm" ? "pick_confirm" : "pick_cancel");
  } catch {
    /* no active value picker */
  }
};

export const layoutBuiltins: Builtins = {
  "panel:split": async () => {
    await invoke("panel_split");
  },
  "panel:unsplit": async () => {
    await invoke("panel_unsplit");
  },
  "panel:hide": async (_args, context) => {
    await invoke("slot_hide", { slot: context.slot });
  },
  "panel:toggle": async (args) => {
    if (args[0] === "split") await invoke("panel_split_toggle");
    else if (args[0] === "fullscreen")
      await setFullscreen(!store.ui.fullscreen);
    else
      await status(
        `unknown flag: "${args[0] ?? ""}" (expected split / fullscreen)`,
      );
  },
  "panel:focus": async (args) => {
    // `next` is the other slot; `left`/`right` name one outright, so a
    // keybinding can reach a slot without knowing which one holds the focus.
    if (args[0] === undefined || args[0] === "next")
      await invoke("panel_focus_next");
    else if (args[0] === "left" || args[0] === "right")
      await invoke("focus_slot", { slot: args[0] });
    else
      await status(
        `unknown slot: "${args[0]}" (expected next / left / right)`,
      );
  },
  "panel:open": async (args, context) => {
    // `panel:open <here|other|new> <type>` (doc "Panel slots and layout").
    const [where, panelType] = args;
    if (!isTarget(where)) {
      await status(
        `unknown target: "${where ?? ""}" (expected ${TARGETS.join(" / ")})`,
      );
      return;
    }
    if (!panelType) return;
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
      return;
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
  },
  "slot:show": async (args, context) => {
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
  },
  "panel:swap": async () => {
    await invoke("panel_swap");
  },
  "message:clear": async (_args, context) => {
    const { ws } = context;
    if (ws) await invoke("clear_messages", { wsId: ws });
  },
  "shell:clear": async (_args, context) => {
    const { ws } = context;
    if (ws) await invoke("clear_shell", { wsId: ws });
  },
  "repos:open": async (_args, context) => {
    await invoke("panel_set_type", {
      slot: context.slot,
      panelType: "repos",
    });
  },
  "find:open": (args) => {
    openFind(args[0]);
  },
  "find:next": () => {
    stepFind(1);
  },
  "find:prev": () => {
    stepFind(-1);
  },
  "find:close": () => {
    closeFind();
  },
  "hints:open": () => {
    openHints();
  },
  "status:clear": (_args, context) => {
    // Dismiss the transient status-bar message (and the last-command echo).
    const { ws } = context;
    if (ws) {
      store.status[ws] = { text: "", kind: "info", timeout_ms: null };
      store.lastCommand[ws] = "";
    }
  },
  "pick:confirm": pickAnswer,
  "pick:cancel": pickAnswer,
  "devtools:open": async () => {
    await invoke("open_devtools");
  },
  "quit": async () => {
    if (!confirmClosing(null)) return;
    await invoke("quit");
  },
};
