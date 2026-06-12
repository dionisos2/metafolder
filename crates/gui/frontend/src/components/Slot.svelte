<script lang="ts">
  import { invoke } from '../lib/ipc';
  import { focusedWs, slotPayload, store, workspaceById } from '../lib/store.svelte';
  import type { SlotId } from '../lib/types';

  let { id }: { id: SlotId } = $props();

  const payload = $derived(slotPayload(id));
  const workspace = $derived(workspaceById(payload.workspace_id));
  const isFocused = $derived(store.layout.focused === id);
  const otherVisible = $derived(store.layout[id === 'left' ? 'right' : 'left'].visible);

  async function focusMe() {
    if (!isFocused) await invoke('panel_focus_next');
  }

  async function setType(event: Event) {
    const select = event.currentTarget as HTMLSelectElement;
    const panelType = select.value;
    try {
      await invoke('panel_set_type', { slot: id, panelType });
    } catch (error) {
      // Rejection (e.g. same type in both slots): restore + status.
      select.value = payload.panel_type ?? '';
      const ws = focusedWs();
      if (ws) {
        await invoke('post_status', { wsId: ws, text: String(error), kind: 'error', timeoutMs: 5000 });
      }
    }
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
<section class="slot" class:focused={isFocused} onclick={focusMe} data-slot={id}>
  <header class="slot-header">
    <select
      class="panel-type"
      value={payload.panel_type ?? ''}
      onchange={setType}
      disabled={payload.workspace_id === null}
    >
      {#if payload.panel_type === null}
        <option value="" disabled>choose a panel…</option>
      {/if}
      {#each store.panelTypes as name (name)}
        <option value={name}>{name}</option>
      {/each}
    </select>
    <span class="header-right">
      <span class="repo-indicator" title="active repository">
        {#if workspace?.active_repo}
          {workspace.active_repo.slice(0, 8)}
        {:else}
          no repo
        {/if}
      </span>
      {#if otherVisible}
        <button
          class="slot-button"
          title="exchange the two panel types (panel:swap)"
          onclick={(e) => {
            e.stopPropagation();
            void invoke('panel_swap');
          }}>⇄</button
        >
        <button
          class="slot-button"
          title="hide this panel slot"
          onclick={(e) => {
            e.stopPropagation();
            void invoke('slot_hide', { slot: id });
          }}>×</button
        >
      {:else}
        <button
          class="slot-button"
          title="show the second panel slot (panel:split)"
          onclick={(e) => {
            e.stopPropagation();
            void invoke('panel_split');
          }}>◫</button
        >
      {/if}
    </span>
  </header>
  <div class="slot-body" data-slot-body={id}>
    {#if payload.workspace_id === null}
      <p class="placeholder">No workspace selected</p>
    {:else if payload.panel_type === null}
      <p class="placeholder">Choose a panel type in the header</p>
    {/if}
    <!-- Panel iframes are positioned over this area by PanelHost. -->
  </div>
</section>

<style>
  .slot {
    display: flex;
    flex-direction: column;
    min-width: 0;
    border: 1px solid transparent;
    background: var(--mf-bg, #1e1e24);
  }
  .slot.focused {
    border-color: var(--mf-focus-border, #4c56c4);
  }
  .slot-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 3px 6px;
    background: var(--mf-bg-raised, #26262e);
    flex: none;
  }
  .panel-type {
    font: inherit;
    background: var(--mf-bg, #1e1e24);
    color: var(--mf-fg, #d8d8e0);
    border: 1px solid var(--mf-fg-dim, #8a8a96);
    border-radius: 3px;
  }
  .header-right {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .repo-indicator {
    color: var(--mf-fg-dim, #8a8a96);
    font-family: var(--mf-font-mono, monospace);
    font-size: 0.85em;
  }
  .slot-button {
    border: none;
    border-radius: 3px;
    padding: 0 4px;
    background: transparent;
    color: var(--mf-fg-dim, #8a8a96);
    font: inherit;
    cursor: pointer;
  }
  .slot-button:hover {
    color: var(--mf-fg, #d8d8e0);
    background: var(--mf-bg, #1e1e24);
  }
  .slot-body {
    position: relative;
    flex: 1;
    min-height: 0;
  }
  .placeholder {
    display: flex;
    height: 100%;
    align-items: center;
    justify-content: center;
    color: var(--mf-fg-dim, #8a8a96);
    margin: 0;
  }
</style>
