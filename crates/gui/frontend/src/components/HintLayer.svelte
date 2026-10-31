<script lang="ts">
  // The labels of the hints (doc "Hints"), laid over the panel, and the field the
  // letters go to: `data-mf-focus="hints"`, so no other key takes them. Fixed over
  // the window, so showing them moves nothing under them.
  import { untrack } from 'svelte';
  import { closeHints, typeHint } from '../lib/hints';
  import { focusedWs, store } from '../lib/store.svelte';

  let field = $state<HTMLInputElement | null>(null);

  $effect(() => {
    if (store.ui.hints.open && field) field.focus();
  });

  // The labels mark where things were: they end when the panel under them changes.
  $effect(() => {
    const where = `${store.layout.focused}|${focusedWs()}|${store.layout.left.panel_type}|${store.layout.right.panel_type}`;
    void where;
    untrack(closeHints);
  });

  const onMoved = () => {
    if (store.ui.hints.open) closeHints();
  };
  $effect(() => {
    window.addEventListener('scroll', onMoved, { capture: true, passive: true });
    window.addEventListener('resize', onMoved);
    return () => {
      window.removeEventListener('scroll', onMoved, { capture: true });
      window.removeEventListener('resize', onMoved);
    };
  });
</script>

{#if store.ui.hints.open}
  <div class="hint-layer">
    {#each store.ui.hints.items as item (item.label)}
      {#if item.label.startsWith(store.ui.hints.typed)}
        <span class="hint-label" style:top="{item.top}px" style:left="{item.left}px">
          <span class="typed">{store.ui.hints.typed}</span>{item.label.slice(store.ui.hints.typed.length)}
        </span>
      {/if}
    {/each}
    <div class="hint-bar" data-help-topic="hints">
      click the element labelled
      <input
        bind:this={field}
        data-mf-focus="hints"
        type="text"
        spellcheck="false"
        autocomplete="off"
        oninput={(event) => {
          event.currentTarget.value = typeHint(event.currentTarget.value);
        }}
        onblur={closeHints}
      />
      — Escape cancels
    </div>
  </div>
{/if}

<style>
  .hint-layer {
    position: fixed;
    inset: 0;
    z-index: 1000;
    pointer-events: none;
  }
  .hint-label {
    position: absolute;
    transform: translate(-4px, -4px);
    padding: 0 3px;
    border-radius: 3px;
    font-family: var(--mf-font-mono, monospace);
    font-size: 0.85em;
    font-weight: bold;
    color: var(--mf-warn-fg, #1e1e24);
    background: var(--mf-warn, #e0b050);
    white-space: nowrap;
  }
  .typed {
    opacity: 0.45;
  }
  .hint-bar {
    position: absolute;
    right: 8px;
    bottom: 8px;
    padding: 3px 8px;
    border: 1px solid var(--mf-accent, #6ea8fe);
    border-radius: 3px;
    font-size: 0.85em;
    color: var(--mf-fg-dim, #8a8a96);
    background: var(--mf-bg-raised, #26262e);
    pointer-events: auto;
  }
  input {
    width: 6em;
    font-family: var(--mf-font-mono, monospace);
    color: var(--mf-fg, #d8d8e0);
    background: var(--mf-bg, #1e1e24);
    border: 1px solid var(--mf-accent, #6ea8fe);
    border-radius: 3px;
  }
</style>
