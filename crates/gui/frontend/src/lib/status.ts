// The status line of the focused workspace, as the shell's commands report to
// it (doc "Status bar and task bar").

import { invoke } from "./ipc";
import { focusedWs } from "./store.svelte";

/** Posts a status message on the focused workspace's status bar (and so to
 *  its message log). `keys.ts` reports a `help:key` answer through it. */
export async function status(text: string, kind = "error") {
  const ws = focusedWs();
  if (ws)
    await invoke("post_status", { wsId: ws, text, kind, timeoutMs: 5000 });
}
