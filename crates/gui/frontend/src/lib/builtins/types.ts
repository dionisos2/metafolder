// A builtin command of the shell (doc "Commands"): its handler, by name, in
// the table of its family. `name` tells apart the names sharing a handler.

import type { ExecContext } from "../types";

export type Builtin = (
  args: string[],
  context: ExecContext,
  name: string,
) => unknown;

export type Builtins = Record<string, Builtin>;
