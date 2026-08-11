<script lang="ts">
  import { invoke } from '../lib/ipc';
  import { dispatch } from '../lib/commands';
  import { focusedWs, slotPayload, store, workspaceById } from '../lib/store.svelte';
  import type { SlotId } from '../lib/types';
  import { showMenu } from '../../../panel-shim/menu.js';

  // `chrome` is false in fullscreen, where only the panel body is shown.
  let { id, chrome = true }: { id: SlotId; chrome?: boolean } = $props();

  async function fullscreenMe(event: Event) {
    event.stopPropagation();
    if (!isFocused) await invoke('focus_slot', { slot: id });
    await dispatch('panel:fullscreen');
  }

  const payload = $derived(slotPayload(id));
  const workspace = $derived(workspaceById(payload.workspace_id));
  const isFocused = $derived(store.layout.focused === id);
  const otherVisible = $derived(store.layout[id === 'left' ? 'right' : 'left'].visible);

  async function focusMe() {
    if (!isFocused) await invoke('focus_slot', { slot: id });
  }

  async function setType(panelType: string) {
    if (panelType === payload.panel_type) return; // no-op re-pick
    try {
      await invoke('panel_set_type', { slot: id, panelType });
    } catch (error) {
      // Rejection (e.g. same type in both slots): report it (no widget state to
      // restore — the header just keeps showing the current type).
      const ws = focusedWs();
      if (ws) {
        await invoke('post_status', { wsId: ws, text: String(error), kind: 'error', timeoutMs: 5000 });
      }
    }
  }

  // Custom dropdown (replacing the native <select>, whose popup WebKitGTK draws
  // itself and cannot be themed): the shared HTML menu from panel-shim, so it
  // carries the app theme, keyboard navigation, typeahead and our scrollbar.
  async function openTypeMenu(event: MouseEvent) {
    event.stopPropagation();
    if (payload.workspace_id === null) return;
    if (!isFocused) await invoke('focus_slot', { slot: id });
    const current = payload.panel_type;
    const items: Metafolder.MenuItem[] = store.panelTypes.map((name) => ({
      // A trailing ✓ marks the current type; kept trailing so the menu's
      // prefix typeahead still matches on the plain name.
      label: name === current ? `${name} ✓` : name,
      action: () => void setType(name),
    }));
    const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
    await showMenu(items, { x: rect.left, y: rect.bottom + 2 });
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
<section class="slot" class:focused={isFocused} onclick={focusMe} data-slot={id}>
  {#if chrome}
    <header class="slot-header" data-help-topic="layout">
    <button
      type="button"
      class="panel-type"
      class:unset={payload.panel_type === null}
      data-help-topic="panel-type"
      onclick={openTypeMenu}
      disabled={payload.workspace_id === null}
    >
      <span class="panel-type-label">{payload.panel_type ?? 'choose a panel…'}</span>
      <span class="panel-type-chevron" aria-hidden="true">▾</span>
    </button>
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
      <button
        class="slot-button"
        title="show only this panel fullscreen (panel:fullscreen; escape exits)"
        onclick={fullscreenMe}>⛶</button
      >
    </span>
    </header>
  {/if}
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
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font: inherit;
    background: var(--mf-bg, #1e1e24);
    color: var(--mf-fg, #d8d8e0);
    border: 1px solid var(--mf-fg-dim, #8a8a96);
    border-radius: 3px;
    padding: 1px 6px;
    cursor: pointer;
  }
  .panel-type:hover:not(:disabled) {
    border-color: var(--mf-accent, #4c56c4);
  }
  .panel-type:disabled {
    cursor: default;
    opacity: 0.55;
  }
  .panel-type.unset .panel-type-label {
    color: var(--mf-fg-dim, #8a8a96);
  }
  .panel-type-chevron {
    font-size: 0.7em;
    opacity: 0.8;
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
