// The shell's builtin commands (doc "Commands"): one table per family, each
// beside the helpers only its commands use. Registered on the Rust side
// (`register_builtins`), handled here.

import { editingBuiltins } from "./editing";
import { helpBuiltins } from "./help";
import { layoutBuiltins } from "./layout";
import { repositoryBuiltins } from "./repository";
import { scriptsBuiltins } from "./scripts";
import type { Builtins } from "./types";
import { workspaceBuiltins } from "./workspace";

/** The families' tables. A function, read on first use rather than at load:
 *  some families import the dispatcher, which imports this module back. */
export function builtinFamilies(): Record<string, Builtins> {
  return {
    editing: editingBuiltins,
    workspace: workspaceBuiltins,
    layout: layoutBuiltins,
    help: helpBuiltins,
    scripts: scriptsBuiltins,
    repository: repositoryBuiltins,
  };
}

let table: Builtins | null = null;

/** Every builtin, by name. */
export function builtins(): Builtins {
  table ??= Object.assign({}, ...Object.values(builtinFamilies())) as Builtins;
  return table;
}
