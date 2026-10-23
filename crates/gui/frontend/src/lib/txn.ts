// Command transactions (doc "Command transactions"): the Tauri calls of a user
// command running in one. Its variable calls carry the transaction — they read
// its snapshot and add to its held writes, on the Rust side — and what it does
// not carry is preceded by a flush, since a panel is about to read the
// variables.

type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

/** The variable calls: they take the transaction. */
const VARIABLE_CALLS = new Set([
  'ws_get_var',
  'ws_set_var',
  'ws_vars',
  'workspace_send',
  'global_get_var',
  'global_vars',
  'global_send',
]);

/** The calls a transaction carries as they are: they change nothing a panel
 *  reads from the variables — the daemon (whose changes the panels follow
 *  through the change feed), the status bar and message log, and reads. */
const CARRIED_CALLS = new Set([
  'daemon_request',
  'daemon_abort',
  'post_status',
  'append_message',
  'get_messages',
  'get_shell_log',
  'parse_query',
  'expand_query',
  'grammar_source',
  'ref_seed',
  'label_separator',
  'get_compiled_keybindings',
  'history_read',
  'history_append',
  'fs_stat',
  'fs_exists',
  'fs_read_dir',
  'fs_home_dir',
  'recent_read',
  'ignore_presets',
  'ignore_current',
  'bench_record',
]);

/**
 * `invoke` within the transaction `txn()` returns — read at each call, so an
 * API outside any (`undefined`) passes every call as is.
 */
export function transactional(
  invoke: Invoke,
  txn: () => number | undefined,
  flush: (txn: number) => Promise<void>,
): Invoke {
  return async (command, args) => {
    const id = txn();
    if (id === undefined || CARRIED_CALLS.has(command)) return invoke(command, args);
    if (VARIABLE_CALLS.has(command)) return invoke(command, { ...args, txn: id });
    await flush(id);
    return invoke(command, args);
  };
}
