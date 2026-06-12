<script lang="ts">
  import { store, workspaceById } from '../lib/store.svelte';

  // One bar when both visible slots show the same workspace; two
  // otherwise (spec-gui "Status bar").
  const barWorkspaces = $derived.by(() => {
    const ids: string[] = [];
    for (const slot of [store.layout.left, store.layout.right]) {
      if (slot.visible && slot.workspace_id !== null && !ids.includes(slot.workspace_id)) {
        ids.push(slot.workspace_id);
      }
    }
    return ids;
  });

  // Pending key sequence: one entry per continuation, sorted by keys.
  const keyHints = $derived.by(() => {
    const pending = store.ui.pendingKeys;
    if (!pending) return null;
    const hints = pending.candidates.map((c) => ({
      keys: c.keys.slice(pending.prefix.length).join(' '),
      invocation: c.invocation,
    }));
    hints.sort((a, b) => a.keys.localeCompare(b.keys));
    return { prefix: pending.prefix.join(' '), hints };
  });
</script>

{#if keyHints}
  <div class="key-hints">
    <span class="prefix">{keyHints.prefix}</span>
    {#each keyHints.hints as hint (hint.keys + hint.invocation)}
      <span class="hint"><span class="keys">{hint.keys}</span> {hint.invocation}</span>
    {/each}
    <span class="hint dim">escape cancels</span>
  </div>
{/if}

<footer class="status-bars">
  {#each barWorkspaces as wsId (wsId)}
    {@const status = store.status[wsId]}
    <div class="status-bar" class:error={status?.kind === 'error'}>
      <span class="ws-name">{workspaceById(wsId)?.name ?? wsId}</span>
      <span class="text">
        {#if status}{status.text}{/if}
      </span>
      {#if status?.kind === 'busy'}<span class="spinner"></span>{/if}
      <span class="last-command">{store.lastCommand[wsId] ?? ''}</span>
    </div>
  {:else}
    <div class="status-bar"><span class="text"></span></div>
  {/each}
</footer>

<style>
  .key-hints {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px 14px;
    padding: 3px 8px;
    background: var(--mf-bg-raised, #26262e);
    border-top: 1px solid var(--mf-bg, #1e1e24);
    font-size: 0.9em;
  }
  .key-hints .prefix,
  .key-hints .keys {
    font-family: var(--mf-font-mono, monospace);
    color: var(--mf-accent, #4c56c4);
  }
  .key-hints .hint.dim {
    margin-left: auto;
    color: var(--mf-fg-dim, #8a8a96);
  }
  .status-bars {
    display: flex;
    flex: none;
    background: var(--mf-bg-raised, #26262e);
    border-top: 1px solid var(--mf-bg, #1e1e24);
  }
  .status-bar {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 2px 8px;
    min-height: 1.4em;
    font-size: 0.9em;
  }
  .status-bar + .status-bar {
    border-left: 1px solid var(--mf-bg, #1e1e24);
  }
  .ws-name {
    color: var(--mf-fg-dim, #8a8a96);
  }
  .text {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .status-bar.error .text {
    color: var(--mf-error, #c44c56);
  }
  .last-command {
    color: var(--mf-fg-dim, #8a8a96);
    font-family: var(--mf-font-mono, monospace);
    font-size: 0.85em;
  }
  .spinner {
    width: 0.8em;
    height: 0.8em;
    border: 2px solid var(--mf-fg-dim, #8a8a96);
    border-top-color: transparent;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
