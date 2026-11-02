import { CommandError, type InvocationParts, registerArgs } from "./commandArgs";
import { parseInvocation } from "./commandParsing";
import { type DispatchResult } from "./commands";
import { type CompletionResult, type CompletionView } from "./completions";
import { GENERIC_COMMANDS } from "./panelFunctions";
import { focusedContext, store } from "./store.svelte";
import { type ExecContext } from "./types";

// ── User commands (doc "User commands") ───────────────────────────────
// Commands defined in ~/.config/metafolder/gui/commands.js. They are ordinary
// *builtins registered at runtime* — no owner, no panel — so they need a
// handler table of their own, looked up after the builtin switch (a user
// command must never shadow a builtin) and before the panel fallback.

/** One command as `commands.js` writes it: the key is the name. */
export interface UserCommand {
  label?: string;
  log?: boolean;
  /** Like ArgSpec, but every function is handed the API first. */
  args?: {
    name: string;
    optional?: boolean;
    prompt?: (mf: unknown, prior: string[]) => string | Promise<string>;
    initial?: (mf: unknown, prior: string[]) => string | Promise<string>;
    complete?: UserCompletionFn | UserCompletionView[];
    /** The cycled views, when which ones exist depends on the prior answers
     *  (doc "Completion views"). Wins over `complete` unless empty. */
    views?: (
      mf: unknown,
      prior: string[],
    ) => CompletionView[] | Promise<CompletionView[]>;
    when?: (mf: unknown, prior: string[]) => boolean;
  }[];
  run: (mf: unknown, ...args: string[]) => unknown;
}

/** A user command's completion builder: like `CompletionFn`, handed the API
 *  first. */
export type UserCompletionFn = (
  mf: unknown,
  partial: string,
  prior: string[],
) => CompletionResult | Promise<CompletionResult>;

/** One cycled view of a user command's completion (doc "Completion
 *  views"). */
export interface UserCompletionView {
  title?: string;
  items: UserCompletionFn;
}

const userHandlers = new Map<
  string,
  (context: ExecContext, ...args: string[]) => unknown
>();

// Set by the loader at boot. Late-bound because the loader imports this module
// (for installUserCommands), so importing it back would be a cycle.
export let reloadUserCommands: () => Promise<unknown> = async () => {};

/** Registers how `config:reload commands` re-imports the user module. */
export function setUserCommandReloader(reload: () => Promise<unknown>): void {
  reloadUserCommands = reload;
}

/**
 * Checks a user module's default export and returns its entries.
 *
 * Separate from installing so a caller can find out a file is malformed
 * *before* tearing down the commands a previous version installed: a bad edit
 * should cost you the reload, not every command you had.
 *
 * Throws naming the offending command; at boot the rejection stops the GUI.
 */
export function validateUserCommands(
  module: unknown,
  file = "commands.js",
): [string, UserCommand][] {
  if (typeof module !== "object" || module === null || Array.isArray(module)) {
    throw new Error(
      `${file} must default-export an object of command definitions`,
    );
  }
  const entries = Object.entries(module as Record<string, UserCommand>);
  for (const [name, command] of entries) {
    if (typeof (command as UserCommand | undefined)?.run !== "function") {
      throw new Error(`${file}: "${name}" has no \`run\` function`);
    }
  }
  return entries;
}

/**
 * Joins the two command modules (doc "User commands"): the shipped
 * `commands.js` and the user's `user-commands.js`, each validated under its
 * own name. A user entry named like a shipped one is left out and returned in
 * `shadowed` — an entry adds to the command set, never replaces part of it
 * unnoticed; a shipped entry is changed in `commands.js` itself.
 */
export function mergeCommandModules(
  shipped: unknown,
  own: unknown,
  saved: unknown = {},
): {
  module: Record<string, UserCommand>;
  shadowed: { file: string; name: string }[];
} {
  const module = Object.fromEntries(
    validateUserCommands(shipped, "commands.js"),
  );
  const shadowed: { file: string; name: string }[] = [];
  // In order: an entry of a later file never replaces one of an earlier.
  for (const [file, source] of [
    ["user-commands.js", own],
    ["macros.js", saved],
  ] as const) {
    for (const [name, command] of validateUserCommands(source, file)) {
      if (Object.hasOwn(module, name)) shadowed.push({ file, name });
      else module[name] = command;
    }
  }
  return { module, shadowed };
}

export async function installUserCommands(
  module: unknown,
  mf: unknown,
  register: (
    name: string,
    label: string,
    log: boolean,
  ) => Promise<boolean | void>,
  onRefused: (name: string) => void = () => {},
  // The API a command's `run` gets, for the context it runs in; `mf` (the one
  // following the focus) serves the argument prompts and completions.
  mfFor: (context: ExecContext) => unknown = () => mf,
): Promise<string[]> {
  const names: string[] = [];
  for (const [name, command] of validateUserCommands(module)) {
    // Registered first: a name the registry refuses is a builtin's, and the
    // entry must then install nothing at all — its argument spec would replace
    // the builtin's, and a reload would forget the builtin with it.
    if (
      (await register(name, command.label ?? name, command.log ?? true)) ===
      false
    ) {
      onRefused(name);
      continue;
    }
    if (command.args) {
      registerArgs(
        name,
        command.args.map((spec) => ({
          name: spec.name,
          optional: spec.optional,
          prompt: (prior: string[]) => spec.prompt?.(mf, prior) ?? spec.name,
          ...(spec.initial
            ? { initial: (prior: string[]) => spec.initial!(mf, prior) }
            : {}),
          ...(spec.complete
            ? Array.isArray(spec.complete)
              ? {
                  complete: spec.complete.map((view) => ({
                    title: view.title,
                    items: (partial: string, prior: string[]) =>
                      view.items(mf, partial, prior),
                  })),
                }
              : {
                  complete: (partial: string, prior: string[]) =>
                    (spec.complete as UserCompletionFn)(mf, partial, prior),
                }
            : {}),
          ...(spec.views
            ? { views: (prior: string[]) => spec.views!(mf, prior) }
            : {}),
          ...(spec.when
            ? { when: (prior: string[]) => spec.when!(mf, prior) }
            : {}),
        })),
      );
    }
    userHandlers.set(name, (context, ...args: string[]) =>
      command.run(mfFor(context), ...args),
    );
    names.push(name);
  }
  return names;
}

/**
 * Lifts `commands.invoke` to the top of the API object.
 *
 * Composing existing commands is what a user command is *for*, so `mf.invoke`
 * is the one call that should not need a path through the object. Panels keep
 * `commands.invoke` and get this too — one alias, not two APIs.
 */
export function withTopLevelInvoke<
  T extends { commands: { invoke: (parts: InvocationParts) => Promise<void> } },
>(api: T): T & { invoke: (parts: InvocationParts) => Promise<void> } {
  return Object.assign(
    Object.create(Object.getPrototypeOf(api) as object) as T,
    api,
    {
      invoke: (parts: InvocationParts) => api.commands.invoke(parts),
    },
  );
}

/** Keeps an uncaught `CommandError` off the console: a panel's button runs
 *  `void commands.invoke([...])`, and when the command fails the status bar
 *  has already said why. Any other rejection is left alone. Returns the
 *  function removing the filter. */
export function ignoreUncaughtCommandErrors(target: EventTarget): () => void {
  const filter = (event: Event) => {
    if (
      (event as Event & { reason?: unknown }).reason instanceof CommandError
    ) {
      event.preventDefault();
    }
  };
  target.addEventListener("unhandledrejection", filter);
  return () => target.removeEventListener("unhandledrejection", filter);
}

/** How `invoke` is written as an array — for the error refusing a string. */
function asParts(invocation: string): string {
  const parsed = parseInvocation(invocation);
  const parts =
    parsed === null
      ? []
      : "shell" in parsed
        ? [`!${parsed.shell}`]
        : [parsed.name, ...parsed.args];
  return `[${parts.map((p) => `'${p.replace(/\\/g, "\\\\").replace(/'/g, "\\'")}'`).join(", ")}]`;
}

/**
 * `mf.invoke` over a dispatcher (doc "User commands"): takes the invocation as
 * an array, resolves once the command has finished, and throws a
 * `CommandError` when it failed or was abandoned — so a sequence of commands
 * stops at the first that did not run.
 */
export function invokerOver(
  dispatchFn: (parts: InvocationParts) => Promise<unknown>,
): (parts: InvocationParts) => Promise<void> {
  return async (parts: InvocationParts) => {
    if (!Array.isArray(parts)) {
      const shown = typeof parts === "string" ? asParts(parts) : String(parts);
      throw new TypeError(
        `invoke takes an array of the name and its arguments: ${shown}`,
      );
    }
    const result = (await dispatchFn(parts)) as DispatchResult;
    if (!result.ok) throw new CommandError(result.error);
    if (result.cancelled)
      throw new CommandError(`${String(parts[0])}: abandoned`, true);
  };
}

/** Whether `name` is an installed user command — never a builtin's name,
 *  which the registry refuses (`installUserCommands`). */
export function isUserCommand(name: string): boolean {
  return userHandlers.has(name);
}

/** Whether `name` runs in a transaction (doc "Command transactions"): a
 *  user command, a panel's command, a generic one, and `slot:show` — no other
 *  builtin. */
export function runsInTransaction(name: string): boolean {
  if (name === "slot:show") return true;
  if (isUserCommand(name) || GENERIC_COMMANDS.includes(name)) return true;
  return store.commands.some(
    (command) => command.name === name && command.owner,
  );
}

/** Runs a user command; false when no such command is installed. */
export async function runUserCommand(
  name: string,
  args: string[],
  context: ExecContext = focusedContext(),
): Promise<boolean> {
  const handler = userHandlers.get(name);
  if (!handler) return false;
  await handler(context, ...args);
  return true;
}

/**
 * Drops one user command because a panel registers a command of that name;
 * false when there was none. Panels mount after `commands.js` is loaded, so
 * this is where such an entry is refused: left in place it would run instead
 * of the panel's command (it is looked up first) under the panel's label.
 */
export function dropUserCommand(name: string): boolean {
  if (!userHandlers.delete(name)) return false;
  registerArgs(name, []);
  return true;
}

/** Drops every installed user command, for `config:reload commands`. */
export function clearUserCommands(): string[] {
  const names = [...userHandlers.keys()];
  for (const name of names) registerArgs(name, []);
  userHandlers.clear();
  return names;
}
