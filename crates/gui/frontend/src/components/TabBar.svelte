<script lang="ts">
  import { invoke } from '../lib/ipc';
  import { dispatch } from '../lib/commands';
  import { store } from '../lib/store.svelte';
  import type { SlotId } from '../lib/types';

  let renaming = $state<string | null>(null);
  let renameDraft = $state('');

  function indicator(wsId: string): { focused: boolean; other: boolean } {
    const focusedSlot = store.layout[store.layout.focused];
    const otherSlot = store.layout[store.layout.focused === 'left' ? 'right' : 'left'];
    return {
      focused: focusedSlot.visible && focusedSlot.workspace_id === wsId,
      other: otherSlot.visible && otherSlot.workspace_id === wsId,
    };
  }

  async function assign(wsId: string, slot: SlotId) {
    try {
      await invoke('tab_assign', { wsId, slot });
    } catch {
      /* slot errors surface via the status bar */
    }
  }

  function otherSlot(): SlotId {
    return store.layout.focused === 'left' ? 'right' : 'left';
  }

  function startRename(wsId: string, current: string) {
    renaming = wsId;
    renameDraft = current;
  }

  async function commitRename() {
    if (renaming !== null && renameDraft.trim() !== '') {
      await invoke('tab_rename', { wsId: renaming, name: renameDraft.trim() });
    }
    renaming = null;
  }
</script>

<nav class="tab-bar">
  {#each store.workspaces as ws (ws.id)}
    {@const ind = indicator(ws.id)}
    <button
      class="tab"
      class:in-focused={ind.focused}
      class:in-other={ind.other}
      onclick={() => assign(ws.id, store.layout.focused)}
      oncontextmenu={(e) => {
        e.preventDefault();
        void assign(ws.id, otherSlot());
      }}
      ondblclick={() => startRename(ws.id, ws.name)}
    >
      {#if renaming === ws.id}
        <!-- svelte-ignore a11y_autofocus -->
        <input
          class="rename"
          autofocus
          bind:value={renameDraft}
          onblur={commitRename}
          onkeydown={(e) => {
            if (e.key === 'Enter') void commitRename();
            if (e.key === 'Escape') renaming = null;
          }}
        />
      {:else}
        <span class="dot dot-focused" class:on={ind.focused}></span>
        <span class="dot dot-other" class:on={ind.other}></span>
        {ws.name}
      {/if}
    </button>
  {/each}
  <button class="tab new-tab" title="tab:new" onclick={() => void dispatch('tab:new')}>+</button>
</nav>

<style>
  .tab-bar {
    display: flex;
    gap: 2px;
    padding: 2px 4px 0;
    background: var(--mf-bg-raised, #26262e);
    overflow-x: auto;
    flex: none;
  }
  .tab {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    border: none;
    border-radius: 4px 4px 0 0;
    padding: 4px 10px;
    background: var(--mf-bg, #1e1e24);
    color: var(--mf-fg, #d8d8e0);
    font: inherit;
    cursor: pointer;
    white-space: nowrap;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: transparent;
  }
  .dot-focused.on {
    background: var(--mf-tab-focused, #56c44c);
  }
  .dot-other.on {
    background: var(--mf-tab-unfocused, #c44c56);
  }
  .new-tab {
    color: var(--mf-fg-dim, #8a8a96);
  }
  .rename {
    font: inherit;
    width: 10em;
  }
</style>
