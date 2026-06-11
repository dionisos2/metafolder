// Thin indirection over the Tauri API so the rest of the shell has a
// single import point (and tests can mock it).

export { invoke } from '@tauri-apps/api/core';
export { listen } from '@tauri-apps/api/event';
