// The in-app help builtins (doc "In-app help"): `help:*`.

import { setHelpCursor } from "../cursor";
import { invoke } from "../ipc";
import { status } from "../status";
import { flashStatus, store } from "../store.svelte";
import type { ExecContext } from "../types";
import type { Builtins } from "./types";

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

export const helpBuiltins: Builtins = {
  "help:open": async (args, context) => {
    const { ws } = context;
    if (ws) await openHelp(ws, context.slot, args.join(" "), true);
  },
  "help:command": async (args, context) => {
    // The command's page (doc "In-app help"): what `help:open <name>` finds,
    // asked with the registered commands as completions. A name nothing
    // registered is refused rather than searched for.
    const { ws } = context;
    const name = args[0] ?? "";
    if (!store.commands.some((c) => c.name === name)) {
      await status(`no command named ${name}`);
      return;
    }
    if (ws) await openHelp(ws, context.slot, name);
  },
  "help:cursor": () => {
    // Arm the `?` cursor: the next click (or escape) is intercepted in keys.ts.
    // One help gesture at a time: arming it drops a pending describe-key wait.
    store.ui.describeKeys = null;
    store.ui.pendingKeys = null;
    store.ui.helpCursorActive = true;
    setHelpCursor(true);
  },
  "help:key": () => {
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
  },
};
