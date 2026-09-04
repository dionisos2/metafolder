<script lang="ts">
  // The find bar (spec-gui "Find in panel"): Ctrl-F over the focused panel's
  // rendered text, the way a browser's own find works. Shown only while the
  // search is open; the input carries `data-mf-focus="find"` so its Enter /
  // Escape / step keys are focus-scoped keybindings (see keybindings.toml)
  // rather than hard-coded here.
  import { untrack } from 'svelte';
  import { closeFind, runFind } from '../lib/find';
  import { focusedWs, store } from '../lib/store.svelte';

  let element = $state<HTMLInputElement | null>(null);

  // Ctrl-F while the bar is already open re-focuses and selects the needle, so
  // a second press starts a new search without reaching for the mouse.
  $effect(() => {
    void store.ui.find.focusTick;
    untrack(() => {
      if (store.ui.find.open && element) {
        element.focus();
        element.select();
      }
    });
  });

  // The search follows the focus: switching slot or workspace puts another
  // panel under the bar, and the matches of the previous one are gone. Only
  // *where* we look is a dependency — `untrack` keeps the search's own writes
  // (count, index) out of it, which would otherwise re-enter this effect.
  $effect(() => {
    const where = `${store.layout.focused}|${focusedWs()}|${store.layout.left.panel_type}|${store.layout.right.panel_type}`;
    void where;
    untrack(() => {
      if (store.ui.find.open) runFind(store.ui.find.needle);
    });
  });

  const position = $derived(
    store.ui.find.needle === ''
      ? ''
      : store.ui.find.count === 0
        ? 'no match'
        : `${store.ui.find.index + 1}/${store.ui.find.count}`,
  );
</script>

{#if store.ui.find.open}
  <div class="find-bar" data-help-topic="find">
    <span class="label">Find</span>
    <input
      bind:this={element}
      data-mf-focus="find"
      type="text"
      spellcheck="false"
      autocomplete="off"
      value={store.ui.find.needle}
      oninput={(event) => runFind((event.currentTarget as HTMLInputElement).value)}
    />
    <span class="position" class:empty={store.ui.find.count === 0 && store.ui.find.needle !== ''}>
      {position}
    </span>
    <span class="hint">enter/shift+enter · escape</span>
    <button type="button" onclick={() => closeFind()}>✕</button>
  </div>
{/if}

<style>
  .find-bar {
    display: flex;
    flex: none;
    align-items: center;
    gap: 8px;
    padding: 3px 10px;
    background: var(--mf-bg-raised, #26262e);
    border-top: 1px solid var(--mf-accent, #4c56c4);
    font-size: 0.9em;
  }
  .label {
    color: var(--mf-fg-dim, #8a8a96);
  }
  input {
    flex: 1;
    min-width: 0;
    background: var(--mf-bg, #1e1e24);
    color: var(--mf-fg, #d8d8e0);
    border: 1px solid var(--mf-fg-dim, #8a8a96);
    border-radius: 3px;
    padding: 1px 6px;
    font-family: var(--mf-font-mono, monospace);
  }
  input:focus {
    outline: none;
    border-color: var(--mf-focus-border, #4c56c4);
  }
  .position {
    color: var(--mf-fg-dim, #8a8a96);
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .position.empty {
    color: var(--mf-error, #c44c56);
  }
  .hint {
    color: var(--mf-fg-dim, #8a8a96);
    white-space: nowrap;
  }
  button {
    background: none;
    border: none;
    color: var(--mf-fg-dim, #8a8a96);
    cursor: pointer;
    padding: 0 4px;
  }
</style>
