// The script and macro builtins (doc "Script sessions", doc "Macros"):
// `script:*`, `answer:send`, `script-keys:toggle`, `macro:*`.

import { registerArgs } from "../commandArgs";
import { reloadUserCommands } from "../commandRegistry";
import { dispatch, runShell, surfacePanel } from "../commands";
import { invoke } from "../ipc";
import { macroEntry, macros } from "../macro";
import { shellQuote } from "../placeholders";
import { status } from "../status";
import { activeQuestion, store } from "../store.svelte";
import type { ExecContext } from "../types";
import type { Builtins } from "./types";

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

registerArgs("script:run", [
  {
    name: "script",
    prompt: () => "Run script:",
    complete: () => scriptCandidates(),
  },
]);

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

export const scriptsBuiltins: Builtins = {
  "script:run": async (args, context) => {
    // The `script` argument was collected by dispatch (with completion);
    // args[0] is the picked "<name> — <summary>" line (or a bare name).
    const choice = args.join(" ").trim();
    if (!choice) return;
    const path = await resolveScriptPath(choice);
    if (!path) {
      // Where to look, and how to start one, for whoever tried a name
      // (doc "mf script").
      await status(
        `no installed script matches "${choice}" (mf script lists them; mf script new starts one from a template)`,
      );
      return;
    }
    await runScript(path, context);
  },
  "answer:send": async (args) => {
    // Resolves a script's POST /gui/input wait: the question on screen
    // (doc "Script sessions").
    await invoke("answer_send", {
      value: args.join(" "),
      id: activeQuestion()?.id ?? null,
    });
  },
  // Macros (doc "Macros").
  "macro:toggle-recording": async () => {
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
  },
  "macro:ask": async () => {
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
  },
  "macro:run": async () => {
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
        return;
      }
    }
  },
  "macro:save": async (args) => {
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
  },
  "script-keys:toggle": async () => {
    // The question bar's checkbox: hand the script's keys back to the panels,
    // or take them again. Rust owns the flag and re-broadcasts the questions
    // with it (doc "Script sessions").
    const enabled = await invoke<boolean>("script_keys_toggle");
    await status(
      enabled ? "script keys enabled" : "script keys disabled",
      "info",
    );
  },
  "script:stop": async (args) => {
    // Shift+escape during a question, or a manual invocation: end the run (and the
    // question with it). No argument = the script asking the question on
    // screen.
    await invoke("script_stop", {
      task: args[0] ?? null,
      kill: false,
      question: activeQuestion()?.id ?? null,
    });
  },
  "script:kill": async (args) => {
    // Ctrl+escape during a question: the same, with SIGKILL — for a script
    // that did not end on the stop.
    await invoke("script_stop", {
      task: args[0] ?? null,
      kill: true,
      question: activeQuestion()?.id ?? null,
    });
  },
};
